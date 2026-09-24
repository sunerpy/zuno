use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::create_escalated_command_execution_sse_response;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

fn zuno_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut command = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("zuno")?);
    command.env("ZUNO_HOME", codex_home);
    Ok(command)
}

#[test]
fn native_acp_initializes_over_stdio_and_exits_on_eof() -> Result<()> {
    let codex_home = TempDir::new()?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientInfo": {"name": "zuno-cli-test", "version": "0"}
        }
    });

    let output = zuno_command(codex_home.path())?
        .arg("acp")
        .write_stdin(format!("{request}\n"))
        .output()?;

    anyhow::ensure!(
        output.status.success(),
        "ACP process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let response: Value = serde_json::from_str(stdout.trim())?;
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["protocolVersion"], 1);
    assert_eq!(response["result"]["agentInfo"]["name"], "Zuno");

    Ok(())
}

/// Kills the ACP process when a test returns early, so a hung bridge cannot
/// keep the test binary alive.
struct AcpProcess {
    child: Child,
    stdin: std::process::ChildStdin,
    frames: mpsc::Receiver<String>,
}

impl AcpProcess {
    fn spawn(codex_home: &Path) -> Result<Self> {
        let mut child = Command::new(codex_utils_cargo_bin::cargo_bin("zuno")?)
            .arg("acp")
            .env("ZUNO_HOME", codex_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawn zuno acp")?;
        let stdin = child.stdin.take().context("acp stdin")?;
        let stdout = child.stdout.take().context("acp stdout")?;
        let (tx, frames) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            frames,
        })
    }

    fn send(&mut self, frame: Value) -> Result<()> {
        writeln!(self.stdin, "{frame}")?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Reads frames until `accept` returns `Some`, failing instead of hanging when
    /// the bridge stops answering.
    fn read_until<T>(&self, what: &str, mut accept: impl FnMut(&Value) -> Option<T>) -> Result<T> {
        loop {
            let line = self
                .frames
                .recv_timeout(Duration::from_secs(90))
                .with_context(|| format!("timed out waiting for {what} from the ACP bridge"))?;
            let frame: Value = serde_json::from_str(&line)
                .with_context(|| format!("ACP bridge wrote a non-JSON frame: {line}"))?;
            if let Some(value) = accept(&frame) {
                return Ok(value);
            }
        }
    }
}

impl Drop for AcpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A command approval is a round trip: the App Server asks the bridge, the bridge
/// asks the ACP client with `session/request_permission`, and the client's answer
/// must reach the bridge while the turn is still waiting on it. The bridge used
/// to await that answer inside the same loop that reads client frames, so the
/// answer was never read and the turn hung forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_acp_completes_a_turn_after_the_client_answers_a_permission_request() -> Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let responses = vec![
        create_escalated_command_execution_sse_response(
            vec!["echo".to_string(), "acp-approval-roundtrip".to_string()],
            /*workdir*/ None,
            Some(5000),
            "call-approve",
        )?,
        create_final_assistant_message_sse_response("done")?,
    ];
    let server = create_mock_responses_server_sequence(responses).await;
    MockResponsesConfig::new(&server.uri())
        .with_approval_policy("on-request")
        .write(codex_home.path())?;

    let mut acp = AcpProcess::spawn(codex_home.path())?;
    acp.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientInfo": {"name": "zuno-cli-test", "version": "0"},
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}}
        }
    }))?;
    acp.read_until("the initialize response", |frame| {
        (frame["id"] == 1).then(|| frame["result"].clone())
    })?;

    acp.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {"cwd": workspace.path(), "mcpServers": []}
    }))?;
    let session_id = acp.read_until("the session/new response", |frame| {
        (frame["id"] == 2).then(|| frame["result"]["sessionId"].as_str().map(str::to_owned))
    })?;
    let session_id = session_id.context("session/new response omitted sessionId")?;

    acp.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "run the command"}]
        }
    }))?;
    let (permission_id, tool_call) = acp.read_until("session/request_permission", |frame| {
        (frame["method"] == "session/request_permission")
            .then(|| (frame["id"].clone(), frame["params"]["toolCall"].clone()))
    })?;
    assert_eq!(tool_call["kind"], "execute");
    assert_eq!(tool_call["status"], "pending");

    acp.send(json!({
        "jsonrpc": "2.0",
        "id": permission_id,
        "result": {"outcome": {"outcome": "selected", "optionId": "allow_once"}}
    }))?;
    let stop_reason = acp.read_until("the session/prompt response", |frame| {
        (frame["id"] == 3).then(|| frame.clone())
    })?;
    assert_eq!(
        stop_reason["result"]["stopReason"], "end_turn",
        "prompt response: {stop_reason}"
    );
    Ok(())
}

/// Zed-facing parity with the reference `codex-acp` adapter: a new session
/// advertises the whole model catalog (not just the thread's model), permission
/// presets as ACP modes, the collaboration mode as a config option, and pushes
/// the slash-command list right after the response. `/status` is answered by
/// the bridge itself, without a model turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_acp_advertises_catalog_modes_and_slash_commands() -> Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    // The mock provider is only configured so that thread/start succeeds; no
    // request reaches it in this test.
    let server = create_mock_responses_server_sequence(Vec::new()).await;
    MockResponsesConfig::new(&server.uri())
        .with_approval_policy("on-request")
        .write(codex_home.path())?;

    let mut acp = AcpProcess::spawn(codex_home.path())?;
    acp.send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": 1, "clientInfo": {"name": "zuno-cli-test", "version": "0"}}
    }))?;
    acp.read_until("the initialize response", |frame| {
        (frame["id"] == 1).then_some(())
    })?;

    acp.send(json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": workspace.path(), "mcpServers": []}
    }))?;
    let session = acp.read_until("the session/new response", |frame| {
        (frame["id"] == 2).then(|| frame["result"].clone())
    })?;
    let session_id = session["sessionId"]
        .as_str()
        .context("sessionId")?
        .to_owned();
    let models = session["models"]["availableModels"]
        .as_array()
        .context("availableModels")?;
    assert!(
        models.len() > 1,
        "the catalog, not just the current model, is offered: {models:?}"
    );
    assert!(
        models
            .iter()
            .any(|m| m["modelId"] == session["models"]["currentModelId"]),
        "the current model is selectable"
    );
    let mode_ids: Vec<&str> = session["modes"]["availableModes"]
        .as_array()
        .context("availableModes")?
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    for preset in [
        "read-only",
        "workspace-write",
        "agent",
        "strict",
        "agent-full-access",
    ] {
        assert!(
            mode_ids.contains(&preset),
            "{preset} missing from {mode_ids:?}"
        );
    }
    let option_ids: Vec<&str> = session["configOptions"]
        .as_array()
        .context("configOptions")?
        .iter()
        .filter_map(|o| o["id"].as_str())
        .collect();
    assert_eq!(
        option_ids,
        vec!["mode", "collaboration_mode", "model", "reasoning_effort"]
    );

    let commands = acp.read_until("available_commands_update", |frame| {
        (frame["method"] == "session/update"
            && frame["params"]["update"]["sessionUpdate"] == "available_commands_update")
            .then(|| frame["params"]["update"]["availableCommands"].clone())
    })?;
    let names: Vec<&str> = commands
        .as_array()
        .context("availableCommands")?
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    for builtin in [
        "plan", "compact", "review", "status", "skills", "mcp", "goal", "rename",
    ] {
        assert!(names.contains(&builtin), "{builtin} missing from {names:?}");
    }

    acp.send(json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "/status"}]}
    }))?;
    let status = acp.read_until("the /status message", |frame| {
        (frame["method"] == "session/update"
            && frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
            .then(|| {
                frame["params"]["update"]["content"]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned()
            })
    })?;
    assert!(status.contains("**Model:**"), "status text: {status}");
    // The mock config sets sandbox_mode = "read-only" and approval_policy =
    // "on-request", which is exactly the read-only preset.
    assert!(
        status.contains("**Permissions:** read-only"),
        "status text: {status}"
    );
    let response = acp.read_until("the /status prompt response", |frame| {
        (frame["id"] == 3).then(|| frame.clone())
    })?;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");

    // `/plan` flips the collaboration mode without a model turn either.
    acp.send(json!({
        "jsonrpc": "2.0", "id": 4, "method": "session/prompt",
        "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "/plan"}]}
    }))?;
    let plan = acp.read_until("the /plan message", |frame| {
        (frame["method"] == "session/update"
            && frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
            .then(|| {
                frame["params"]["update"]["content"]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned()
            })
    })?;
    assert!(plan.starts_with("Plan mode on"), "{plan}");
    acp.read_until("the /plan prompt response", |frame| {
        (frame["id"] == 4).then_some(())
    })?;
    acp.send(json!({
        "jsonrpc": "2.0", "id": 5, "method": "session/set_config_option",
        "params": {"sessionId": session_id, "configId": "mode", "value": "workspace-write"}
    }))?;
    let options = acp.read_until("the set_config_option response", |frame| {
        (frame["id"] == 5).then(|| frame["result"]["configOptions"].clone())
    })?;
    let mode = options
        .as_array()
        .context("configOptions")?
        .iter()
        .find(|o| o["id"] == "mode")
        .context("mode option")?;
    assert_eq!(mode["currentValue"], "workspace-write");
    let collaboration = options
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "collaboration_mode")
        .unwrap();
    assert_eq!(collaboration["currentValue"], "plan");
    Ok(())
}
